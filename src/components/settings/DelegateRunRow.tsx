import { useState } from 'react';

import { ipc } from '@/lib/ipc';
import type { SpecialistRun, TranscriptRow } from '@/lib/types';

/** One delegate run, expandable to the transcript it produced.
 *
 *  This is the only place a delegate's transcript is reachable. Every
 *  `call_specialist` creates a real session, and a three-way fan-out therefore
 *  used to add three rows to Saved Chats every turn, burying the user's own
 *  conversations; the daemon now filters any session with a `parent_session_id`
 *  out of that listing (`list_sessions_page_for_app`). They are still whole
 *  sessions — just kept here, next to the definitions that produced them,
 *  rather than in the list of the user's own work.
 *
 *  Loaded lazily on first expand: a run's transcript can be long, and most are
 *  never opened. */
export function DelegateRunRow({ run }: { run: SpecialistRun }) {
  const [open, setOpen] = useState(false);
  const [rows, setRows] = useState<TranscriptRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const dot = run.status === 'failed' ? 'bad' : run.status === 'completed' ? 'ok' : 'warn';

  const toggle = async () => {
    const next = !open;
    setOpen(next);
    if (!next || rows || loading || !run.session_id) return;
    setLoading(true);
    setError(null);
    try {
      setRows(await ipc.fetchSessionTranscript(run.session_id));
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="delegate-run">
      <button
        type="button"
        className="row delegate-run-head"
        aria-expanded={open}
        onClick={() => void toggle()}
      >
        <span className={`status-dot ${dot}`} />
        <div style={{ flex: 1 }}>
          <div>{run.specialist ?? 'specialist'}</div>
          <div className="muted" style={{ fontSize: 13 }}>
            {run.summary ?? run.status}
          </div>
        </div>
        <span className="muted" style={{ fontSize: 12 }}>
          {run.started_at ? new Date(run.started_at).toLocaleString() : ''}
        </span>
        <span className="muted delegate-run-caret">{open ? '▾' : '▸'}</span>
      </button>

      {open && (
        <div className="delegate-run-body">
          {loading && <p className="muted">Loading transcript…</p>}
          {error && <p className="chat-error">{error}</p>}
          {!loading && !error && !run.session_id && (
            <p className="muted">This run recorded no session to read.</p>
          )}
          {rows?.length === 0 && <p className="muted">The delegate left no messages.</p>}
          {rows?.map((r, i) => (
            <div className="delegate-run-msg" key={i}>
              <div className="muted delegate-run-role">{r.role}</div>
              {r.text && <div className="delegate-run-text">{r.text}</div>}
              {r.tools.length > 0 && (
                <div className="muted delegate-run-tools">
                  {r.tools.map((t) => (
                    <span className="delegate-run-tool" key={t}>
                      {t}
                    </span>
                  ))}
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
