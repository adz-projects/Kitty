import type { PendingApproval } from '@/lib/types';
import { useApprovalStore } from '@/stores/approvalStore';

/** A human-readable preview of the tool call's arguments: the command for a
    shell call, otherwise the arguments as JSON. */
export function previewArgs(input: unknown): string {
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

/** One approval: the tool, its exact arguments, why it was not answered
    automatically, and what "Always allow" would cover. Nothing runs until
    the user acts (CLAUDE.md Phase 3). Used inline in the chat that asked and
    in the interrupting dialog for any other chat. */
export function ApprovalPrompt({
  approval,
  inline = true,
}: {
  approval: PendingApproval;
  /** Inline in its chat (card chrome) rather than inside a dialog. */
  inline?: boolean;
}) {
  const answer = useApprovalStore((s) => s.answer);
  const busy = useApprovalStore((s) => s.answering.includes(approval.action_id));
  const preview = previewArgs(approval.tool_args);

  // Approving must be deliberate: Enter never approves; a focused button
  // still activates on Space (Phase 8 a11y).
  const noEnter = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') e.preventDefault();
  };
  const pick = (decision: 'allow' | 'always_allow' | 'reject') =>
    void answer(approval.action_id, decision);

  return (
    <div
      className={inline ? 'approval' : 'approval approval-in-dialog'}
      role={inline ? 'alertdialog' : undefined}
      aria-label={inline ? 'Tool approval required' : undefined}
    >
      <div className="approval-head">
        <strong>Approve tool: {approval.tool_name}?</strong>
      </div>
      {preview && <pre className="approval-cmd">{preview}</pre>}
      {approval.warning && <p className="muted approval-note">{approval.warning}</p>}
      {approval.scheduled && (
        <p className="muted approval-note">
          This is a scheduled task. If nobody answers within 10 minutes it is denied and the task
          carries on without it.
        </p>
      )}
      <div className="actions">
        <button
          className="primary"
          disabled={busy}
          onKeyDown={noEnter}
          onClick={() => pick('allow')}
        >
          Approve
        </button>
        <button
          disabled={busy}
          onKeyDown={noEnter}
          onClick={() => pick('always_allow')}
          title="Stop asking for this. You can revoke it in Settings → Tool permissions."
        >
          Always allow {approval.always_scope.label}
        </button>
        <button disabled={busy} onKeyDown={noEnter} onClick={() => pick('reject')}>
          Deny
        </button>
      </div>
    </div>
  );
}
