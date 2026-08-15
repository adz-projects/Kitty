import { useState } from 'react';
import type { ApprovalNeededEvent } from '@/lib/types';

/** Extract a human-readable command/param preview from the tool call. */
function previewInput(input: unknown): string {
  if (input == null) return '';
  if (typeof input === 'string') return input;
  if (typeof input === 'object') {
    const obj = input as Record<string, unknown>;
    if (typeof obj.command === 'string') return obj.command;
    try {
      return JSON.stringify(obj, null, 2);
    } catch {
      return String(input);
    }
  }
  return String(input);
}

/** Inline approval for a tool call that requires permission. Nothing runs until
    the user acts (CLAUDE.md Phase 3). Approve requires an explicit click. */
export function ApprovalPrompt({
  request,
  onRespond,
}: {
  request: ApprovalNeededEvent;
  /** Resolves false when the decision never reached the backend — the prompt
      then unlatches so the user can retry (the store keeps the entry queued
      on failure, so the turn isn't silently hung behind a dead prompt). */
  onRespond: (toolCallId: string, optionId: string | null) => Promise<boolean>;
}) {
  const title = request.tool_call?.title ?? 'a tool';
  const preview = previewInput(request.tool_call?.rawInput);
  const has = (id: string) => request.options?.some((o) => o.optionId === id);
  // Latch on first response so a second click (before the prompt is cleared)
  // can't send a duplicate permission decision to goosed.
  const [submitted, setSubmitted] = useState(false);
  const respond = (optionId: string | null) => {
    if (submitted) return;
    setSubmitted(true);
    void onRespond(request.tool_call_id, optionId).then((ok) => {
      if (!ok) setSubmitted(false);
    });
  };
  const pick = (id: string) => respond(id);

  // A11y (Phase 8): approving must be deliberate — block Enter so a stray Enter
  // can't approve; a focused button still activates on Space.
  const noEnter = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') e.preventDefault();
  };

  return (
    <div className="approval" role="alertdialog" aria-label="Tool approval required">
      <div className="approval-head">
        <strong>Approve tool: {title}?</strong>
      </div>
      {preview && <pre className="approval-cmd">{preview}</pre>}
      <div className="actions">
        {has('allow_once') && (
          <button
            className="primary"
            disabled={submitted}
            onKeyDown={noEnter}
            onClick={() => pick('allow_once')}
          >
            Approve
          </button>
        )}
        {has('allow_always') && (
          <button disabled={submitted} onKeyDown={noEnter} onClick={() => pick('allow_always')}>
            Always allow
          </button>
        )}
        {has('reject_once') ? (
          <button disabled={submitted} onKeyDown={noEnter} onClick={() => pick('reject_once')}>
            Deny
          </button>
        ) : (
          <button disabled={submitted} onKeyDown={noEnter} onClick={() => respond(null)}>
            Deny
          </button>
        )}
      </div>
    </div>
  );
}
