import { useEffect, useMemo, useState } from 'react';
import type { PendingApproval } from '@/lib/types';
import { ipc } from '@/lib/ipc';
import {
  chatsDeleted,
  groupCandidates,
  proposalFromArgs,
  rowFacts,
  type TidyRow,
  type TidyState,
} from '@/lib/tidy';
import { useApprovalStore } from '@/stores/approvalStore';
import { confirmDialog } from '@/components/shared/ConfirmDialog';

/** A `/tidy` list waiting for the user: one checkbox per chat, the chats
    the model would delete checked. Delete removes the checked chats
    permanently (and this tidy chat); "Read these first" lets the model read
    the unsure chats' first and last exchange and propose again; Revise sends
    the user's words back; Cancel ends it. Never answered automatically
    (`approvals::ALWAYS_ASK_TOOLS`). */
export function TidyPrompt({
  approval,
  inline = true,
}: {
  approval: PendingApproval;
  inline?: boolean;
}) {
  const answer = useApprovalStore((s) => s.answer);
  const answering = useApprovalStore((s) => s.answering.includes(approval.action_id));
  const proposal = useMemo(() => proposalFromArgs(approval.tool_args), [approval.tool_args]);
  const [state, setState] = useState<TidyState | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [checked, setChecked] = useState<Set<string>>(() => new Set(proposal.delete));
  const [revising, setRevising] = useState(false);
  const [notes, setNotes] = useState('');
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    ipc
      .getTidy(approval.session_id)
      .then((s) => live && setState(s))
      .catch(() => live && setLoadFailed(true));
    return () => {
      live = false;
    };
  }, [approval.session_id]);

  const groups = useMemo(
    () => (state ? groupCandidates(state.candidates, proposal) : null),
    [state, proposal]
  );
  const busy = answering || working;
  const toRead = (groups?.unsure ?? [])
    .map((r) => r.ref)
    .filter((r) => !state?.readable.includes(r));
  const chosen = [...checked].filter((r) => state?.candidates.some((c) => c.ref === r));
  const count = state ? chatsDeleted(state.candidates, chosen) : 0;

  const toggle = (ref: string) =>
    setChecked((s) => {
      const next = new Set(s);
      if (next.has(ref)) next.delete(ref);
      else next.add(ref);
      return next;
    });

  const noEnter = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') e.preventDefault();
  };

  const deleteChosen = async () => {
    const ok = await confirmDialog({
      title: 'Delete chats permanently?',
      message: `This deletes ${count} chat${count === 1 ? '' : 's'}, with the files in their own chat folders, and then this tidy chat. It can't be undone.`,
      confirmLabel: `Delete ${count}`,
      danger: true,
    });
    if (!ok) return;
    setWorking(true);
    setError(null);
    try {
      await ipc.tidyApply(approval.session_id, approval.action_id, chosen);
    } catch (e) {
      setError(String(e));
      setWorking(false);
    }
  };

  const readFirst = async () => {
    setWorking(true);
    setError(null);
    try {
      await ipc.tidyAllowReading(approval.session_id, approval.action_id, toRead);
    } catch (e) {
      setError(String(e));
      setWorking(false);
    }
  };

  const row = (r: TidyRow) => (
    <li key={r.ref} className="tidy-row">
      <label className="check">
        <input
          type="checkbox"
          checked={checked.has(r.ref)}
          disabled={busy}
          onChange={() => toggle(r.ref)}
        />
        <span className="tidy-row-text">
          <span className="tidy-title">{r.title}</span>
          <span className="muted tidy-facts">{rowFacts(r)}</span>
          {r.note && <span className="tidy-note">{r.note}</span>}
        </span>
      </label>
    </li>
  );

  return (
    <div
      className={inline ? 'approval tidy-plan' : 'approval tidy-plan approval-in-dialog'}
      role={inline ? 'alertdialog' : undefined}
      aria-label={inline ? 'Chats to tidy' : undefined}
    >
      <div className="approval-head">
        <strong>Tidy chats</strong>
      </div>
      {proposal.summary && <p className="tidy-summary">{proposal.summary}</p>}
      <div className="tidy-body">
        {loadFailed ? (
          <p className="muted">This tidy list is no longer available. Run /tidy again.</p>
        ) : !groups ? (
          <p className="muted">Reading the list…</p>
        ) : (
          <>
            <section>
              <h4>Delete ({groups.delete.length})</h4>
              {groups.delete.length ? (
                <ul className="tidy-list">{groups.delete.map(row)}</ul>
              ) : (
                <p className="muted">Nothing proposed for deletion.</p>
              )}
            </section>
            {groups.unsure.length > 0 && (
              <section>
                <h4>Unsure ({groups.unsure.length})</h4>
                <p className="muted">
                  It can read the first and last exchange of these to decide, if you let it. They're
                  read by this chat's model.
                </p>
                <ul className="tidy-list">{groups.unsure.map(row)}</ul>
              </section>
            )}
            {groups.keep.length > 0 && (
              <details>
                <summary>Keep ({groups.keep.length})</summary>
                <ul className="tidy-list">{groups.keep.map(row)}</ul>
              </details>
            )}
          </>
        )}
      </div>
      {error && <div className="chat-error">{error}</div>}
      {revising ? (
        <>
          <label className="research-plan-notes">
            <span className="muted">What should change?</span>
            <textarea
              value={notes}
              rows={3}
              autoFocus
              onChange={(e) => setNotes(e.target.value)}
              placeholder="e.g. keep anything about the op-ed"
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
            className="danger"
            disabled={busy || count === 0}
            onKeyDown={noEnter}
            onClick={() => void deleteChosen()}
          >
            {count === 0 ? 'Delete' : `Delete ${count} chat${count === 1 ? '' : 's'}…`}
          </button>
          {toRead.length > 0 && (
            <button disabled={busy} onKeyDown={noEnter} onClick={() => void readFirst()}>
              Read the unsure ones first
            </button>
          )}
          <button disabled={busy} onKeyDown={noEnter} onClick={() => setRevising(true)}>
            Revise…
          </button>
          <button
            disabled={busy}
            onKeyDown={noEnter}
            onClick={() =>
              void answer(approval.action_id, 'reject', 'The user cancelled the tidy.')
            }
          >
            Cancel
          </button>
        </div>
      )}
    </div>
  );
}
