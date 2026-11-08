import { useEffect, useState } from 'react';
import { ipc, pickSavePath } from '@/lib/ipc';
import type { OvernightReport, OvernightSettings, OvernightView } from '@/lib/types';
import { describeReport, describeWhen, wakeTimerWarning } from './overnightHelpers';

/** Settings → Memorabilia → Overnight learning (docs/MEMORY_OVERNIGHT.md).
    During the day memory only screens what chats bring in; chunking,
    embedding, deciding what has recurred and extracting facts wait for the
    window, so they don't compete with whatever the computer is doing.
    Desktop only. */
export function OvernightLearning() {
  const [view, setView] = useState<OvernightView | null>(null);
  const [draft, setDraft] = useState<OvernightSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [note, setNote] = useState('');

  const load = async () => {
    try {
      const v = await ipc.getOvernight();
      setView(v);
      setDraft((d) => d ?? v.settings);
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    void load();
    // Refresh while open: a pass may start or finish.
    const t = setInterval(() => void load(), 15_000);
    return () => clearInterval(t);
  }, []);

  if (!view || !draft) return error ? <div className="chat-error">{error}</div> : null;
  if (!view.supported) return null;

  const dirty = JSON.stringify(draft) !== JSON.stringify(view.settings);
  const set = (patch: Partial<OvernightSettings>) => setDraft({ ...draft, ...patch });

  const save = async () => {
    setBusy(true);
    setError('');
    setNote('');
    try {
      const saved = await ipc.setOvernight(draft);
      setDraft(saved);
      setNote('Saved.');
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
      void load();
    }
  };

  const runNow = async () => {
    setError('');
    try {
      await ipc.runOvernightNow();
      setNote(
        'Starting within a minute. It runs for up to an hour, even while you use the computer.'
      );
      void load();
    } catch (e) {
      setError(String(e));
    }
  };

  const saveLog = async () => {
    setError('');
    const path = await pickSavePath('screened-paragraphs.jsonl', {
      name: 'JSON Lines',
      extensions: ['jsonl'],
    });
    if (!path) return;
    try {
      const n = await ipc.saveScreenLog(path, 1000);
      setNote(
        `Saved ${n} paragraph${n === 1 ? '' : 's'}. Set "keep" to true or false on each to label it.`
      );
    } catch (e) {
      setError(String(e));
    }
  };

  const status = view.engine?.status;
  const backlog = view.engine?.backlog;
  const report: OvernightReport | null = status?.last_report ?? null;
  const timerWarning = wakeTimerWarning(view.wake_timers);

  return (
    <>
      <h2>Overnight learning</h2>
      <p className="muted">
        During the day Memorabilia only reads what your chats bring in and keeps the paragraphs
        worth remembering. The heavy work (deciding which topics keep coming back and learning facts
        from them) waits for this window, so it doesn&apos;t slow the computer down while you use
        it.
      </p>

      <label className="check">
        <input
          type="checkbox"
          checked={draft.enabled}
          disabled={busy}
          onChange={(e) => set({ enabled: e.target.checked })}
        />
        <span>Learn overnight</span>
      </label>
      {!draft.enabled && (
        <small className="muted">
          Off: memory does this work about once an hour, whenever it is due.
        </small>
      )}

      {draft.enabled && (
        <>
          <div className="row">
            <label>
              From{' '}
              <input
                type="time"
                value={draft.start}
                disabled={busy}
                onChange={(e) => set({ start: e.target.value })}
              />
            </label>
            <label>
              to{' '}
              <input
                type="time"
                value={draft.end}
                disabled={busy}
                onChange={(e) => set({ end: e.target.value })}
              />
            </label>
          </div>
          <small className="muted">
            It starts once you&apos;ve been away for ten minutes and pauses when you come back. If a
            night is missed, it catches up the next time the computer is idle and plugged in.
          </small>

          <label className="check">
            <input
              type="checkbox"
              checked={draft.pathway}
              disabled={busy}
              onChange={(e) => set({ pathway: e.target.checked })}
            />
            <span>Learn about you (Adaptive Pathway) overnight too</span>
          </label>

          <label className="check">
            <input
              type="checkbox"
              checked={draft.wake}
              disabled={busy}
              onChange={(e) => set({ wake: e.target.checked })}
            />
            <span>Wake this computer to do it</span>
          </label>
          <small className="muted">
            Adds a Windows scheduled task that wakes the computer a few minutes after the window
            opens, only when it&apos;s plugged in. It goes back to sleep by itself when the work is
            done. A computer that is shut down rather than asleep can&apos;t be woken.
          </small>
          {draft.wake && timerWarning && <div className="chat-error">{timerWarning}</div>}
          {draft.wake && !dirty && view.settings.wake && !view.wake_task_registered && (
            <div className="chat-error">
              The wake-up task isn&apos;t registered. Save again to retry.
            </div>
          )}
        </>
      )}

      <div className="row">
        <button className="primary" disabled={busy || !dirty} onClick={() => void save()}>
          Save
        </button>
        {view.settings.enabled && (
          <button disabled={busy || status?.running} onClick={() => void runNow()}>
            Process now
          </button>
        )}
      </div>
      {note && <p className="muted">{note}</p>}
      {error && <div className="chat-error">{error}</div>}

      {status && (
        <small className="muted" style={{ display: 'block' }}>
          {status.running
            ? 'Working now.'
            : status.last_started
              ? `Last run ${describeWhen(status.last_started)}${report ? `: ${describeReport(report)}` : '.'}`
              : 'Hasn’t run yet.'}
        </small>
      )}
      {backlog && (
        <small className="muted" style={{ display: 'block' }}>
          Waiting: {backlog.screened} document{backlog.screened === 1 ? '' : 's'} for tonight,{' '}
          {backlog.candidates_waiting} passage{backlog.candidates_waiting === 1 ? '' : 's'} to
          recur, {backlog.pending_extraction} to learn from
          {backlog.unscreened > 0
            ? `, ${backlog.unscreened} for the decision model (dropped after three days)`
            : ''}
          .
        </small>
      )}

      <div className="row">
        <button className="link" onClick={() => void saveLog()}>
          Save screened paragraphs for labelling…
        </button>
      </div>
      <small className="muted">
        For testing decision-model questions: the newest paragraphs memory judged, with each
        question&apos;s score. See docs/MEMORY_OVERNIGHT.md.
      </small>
    </>
  );
}
