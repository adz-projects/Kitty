import { useState } from 'react';
import { createPortal } from 'react-dom';
import type { Message } from '@/stores/chatStore';
import { usePopoverPosition } from '@/lib/usePopoverPosition';

/** `inputTokens` is normalized (bigtiny_rust's `usage_map_from_anthropic`) to
    always mean *total* prompt size, cache included, for every provider — so
    the hit rate is simply the cached share of that total. `cacheReadTokens`/
    `cacheCreationTokens` are independently optional (a turn can report one
    without the other), so both default to 0 for the arithmetic; `inputTokens`
    itself is only missing if the caller didn't check `hasData` first. */
export function formatCacheHitRate(message: Message): string {
  const read = message.cacheReadTokens ?? 0;
  const created = message.cacheCreationTokens ?? 0;
  const total = message.inputTokens ?? 0;
  const parts = [`${read} read`];
  if (created > 0) parts.push(`${created} written`);
  // `inputTokens` can legitimately be missing while cache tokens are present
  // (a partial provider report) — a "of 0" denominator is meaningless and
  // misleading ("0% hit rate (… of 0)"), so say n/a instead of fabricating a
  // percentage for a total we don't actually have.
  if (!(total > 0)) return `${parts.join(', ')} (n/a total)`;
  const pct = Math.round((read / total) * 100);
  return `${pct}% hit rate (${parts.join(', ')} of ${total})`;
}

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
      {open &&
        // Portaled to document.body: inside the virtualized message list each
        // row carries a `transform` (translateY), which becomes the containing
        // block for `position: fixed` descendants — the popover's
        // viewport-relative coordinates would resolve against the ROW instead,
        // landing it off-screen past the virtualization threshold. The portal
        // escapes the transformed ancestor entirely; usePopoverPosition's
        // outside-click handling still works since it refs the portaled node.
        createPortal(
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
            {(message.cacheReadTokens != null || message.cacheCreationTokens != null) && (
              <div>
                <span className="muted">Prompt cache:</span> {formatCacheHitRate(message)}
              </div>
            )}
            {message.ttftMs != null && (
              <div>
                <span className="muted">Time to first token:</span>{' '}
                {(message.ttftMs / 1000).toFixed(2)}s
              </div>
            )}
            {message.ttftMs != null &&
              message.durationMs != null &&
              message.outputTokens != null &&
              message.outputTokens > 0 &&
              message.durationMs > message.ttftMs && (
                <div>
                  <span className="muted">Generation speed:</span>{' '}
                  {((message.outputTokens / (message.durationMs - message.ttftMs)) * 1000).toFixed(
                    1
                  )}{' '}
                  tok/s
                </div>
              )}
          </div>,
          document.body
        )}
    </div>
  );
}
