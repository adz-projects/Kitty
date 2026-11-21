import { useState } from 'react';
import type { PendingApproval } from '@/lib/types';
import { planFromArgs, planMarkdown } from '@/lib/research';
import { useApprovalStore } from '@/stores/approvalStore';
import { MarkdownBlocks } from './MarkdownBlocks';

/** A deep research plan waiting for the user: Approve starts the research,
    Revise sends the user's words back so the model redrafts it, Reject ends
    the run. Never answered automatically (`approvals::ALWAYS_ASK_TOOLS`), and
    no "Always allow": every plan is its own decision. */
export function ResearchPlanPrompt({
  approval,
  inline = true,
}: {
  approval: PendingApproval;
  inline?: boolean;
}) {
  const answer = useApprovalStore((s) => s.answer);
  const busy = useApprovalStore((s) => s.answering.includes(approval.action_id));
  const [revising, setRevising] = useState(false);
  const [notes, setNotes] = useState('');
  const plan = planFromArgs(approval.tool_args);

  const noEnter = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') e.preventDefault();
  };

  return (
    <div
      className={inline ? 'approval research-plan' : 'approval research-plan approval-in-dialog'}
      role={inline ? 'alertdialog' : undefined}
      aria-label={inline ? 'Research plan to approve' : undefined}
    >
      <div className="approval-head">
        <strong>Deep research plan</strong>
      </div>
      <div className="research-plan-body">
        {plan ? (
          <MarkdownBlocks text={planMarkdown(plan)} />
        ) : (
          <p className="muted">The plan could not be read.</p>
        )}
      </div>
      {revising ? (
        <>
          <label className="research-plan-notes">
            <span className="muted">What should change?</span>
            <textarea
              value={notes}
              rows={3}
              autoFocus
              onChange={(e) => setNotes(e.target.value)}
              placeholder="e.g. include radio drama, and stop at 1965"
            />
          </label>
          <div className="actions">
            <button
              className="primary"
              disabled={busy || notes.trim() === ''}
              onKeyDown={noEnter}
              onClick={() => void answer(approval.action_id, 'revise', notes.trim())}
            >
              Send revisions
            </button>
            <button disabled={busy} onKeyDown={noEnter} onClick={() => setRevising(false)}>
              Back
            </button>
          </div>
        </>
      ) : (
        <div className="actions">
          <button
            className="primary"
            disabled={busy || !plan}
            onKeyDown={noEnter}
            onClick={() => void answer(approval.action_id, 'allow')}
          >
            Approve and start
          </button>
          <button disabled={busy} onKeyDown={noEnter} onClick={() => setRevising(true)}>
            Revise…
          </button>
          <button
            disabled={busy}
            onKeyDown={noEnter}
            onClick={() => void answer(approval.action_id, 'reject', 'The user rejected the plan.')}
          >
            Reject
          </button>
        </div>
      )}
    </div>
  );
}
