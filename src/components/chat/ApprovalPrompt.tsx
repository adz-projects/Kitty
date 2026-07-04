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
  onRespond: (toolCallId: string, optionId: string | null) => void;
}) {
  const title = request.tool_call?.title ?? 'a tool';
  const preview = previewInput(request.tool_call?.rawInput);
  const has = (id: string) => request.options?.some((o) => o.optionId === id);
  const pick = (id: string) => onRespond(request.tool_call_id, id);

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
          <button className="primary" onKeyDown={noEnter} onClick={() => pick('allow_once')}>
            Approve
          </button>
        )}
        {has('allow_always') && (
          <button onKeyDown={noEnter} onClick={() => pick('allow_always')}>
            Always allow
          </button>
        )}
        {has('reject_once') ? (
          <button onKeyDown={noEnter} onClick={() => pick('reject_once')}>
            Deny
          </button>
        ) : (
          <button onKeyDown={noEnter} onClick={() => onRespond(request.tool_call_id, null)}>
            Deny
          </button>
        )}
      </div>
    </div>
  );
}
