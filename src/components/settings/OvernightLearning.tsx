import { useEffect, useState } from 'react';
import { ipc, pickSavePath } from '@/lib/ipc';
import type { OvernightReport, OvernightSettings, OvernightView } from '@/lib/types';
import { describeReport, describeWhen, wakeTimerWarning } from './overnightHelpers';

/** Settings → Memorabilia → Learning schedule (desktop only).
    Groups overnight scheduling, daytime idle micro-bursts, and provider
    offload into one readable card, keeping labelling export tucked under
    Developer tools. */
export function OvernightLearning() {
  const [view, setView] = useState<OvernightView | null>(null);
  const [draft, setDraft] = useState<OvernightSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [note, setNote] = useState('');
  const [showDevTools, setShowDevTools] = useState(false);

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
      setNote('Starting within a minute. Runs for up to an hour without blocking chat.');
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
  const hasBacklog = Boolean(
    backlog &&
    (backlog.screened > 0 ||
      backlog.candidates_waiting > 0 ||
      backlog.pending_extraction > 0 ||
      backlog.unscreened > 0)
  );

  return (
    <div className="mem-card">
      <div className="mem-card-head">
        <h2>Learning schedule</h2>
        <div className="mem-card-actions">
          {status && (
            <span className={`mem-pill ${status.running ? 'mem-pill-accent' : ''}`}>
              {status.running
                ? 'Running now'
                : status.last_started
                  ? `Last run ${describeWhen(status.last_started)}`
                  : 'Hasn’t run yet'}
            </span>
          )}
          {view.settings.enabled && (
            <button disabled={busy || status?.running} onClick={() => void runNow()}>
              Process now
            </button>
          )}
        </div>
      </div>

      {status && !status.running && report && (
        <small className="muted">Last summary: {describeReport(report)}</small>
      )}

      {hasBacklog && backlog && (
        <div className="mem-status-strip">
          <span className="mem-pill">{backlog.screened} documents screened</span>
          <span className="mem-pill">{backlog.candidates_waiting} recurring passages</span>
          <span className="mem-pill">{backlog.pending_extraction} pending extraction</span>
          {backlog.unscreened > 0 && (
            <span className="mem-pill mem-pill-warn">{backlog.unscreened} awaiting gate</span>
          )}
        </div>
      )}

      <div className="mem-setting-list">
        <div className="mem-setting-row">
          <label className="check">
            <input
              type="checkbox"
              checked={draft.enabled}
              disabled={busy}
              onChange={(e) => set({ enabled: e.target.checked })}
            />
            <span>Run heavy memory learning overnight</span>
          </label>
          <small className="muted">
            {draft.enabled
              ? 'Waits until idle for 10 min inside this window and pauses immediately when you return.'
              : 'Off: processes background batches about once an hour when due.'}
          </small>

          {draft.enabled && (
            <div className="mem-time-row">
              <span>Window</span>
              <input
                aria-label="Overnight window start"
                type="time"
                value={draft.start}
                disabled={busy}
                onChange={(e) => set({ start: e.target.value })}
              />
              <span>to</span>
              <input
                aria-label="Overnight window end"
                type="time"
                value={draft.end}
                disabled={busy}
                onChange={(e) => set({ end: e.target.value })}
              />
            </div>
          )}
        </div>

        {draft.enabled && (
          <div className="mem-suboptions">
            <div className="mem-setting-row">
              <label className="check">
                <input
                  type="checkbox"
                  checked={draft.pathway}
                  disabled={busy}
                  onChange={(e) => set({ pathway: e.target.checked })}
                />
                <span>Include Adaptive Pathway overnight</span>
              </label>
            </div>

            <div className="mem-setting-row">
              <label className="check">
                <input
                  type="checkbox"
                  checked={draft.idle_micro ?? true}
                  disabled={busy}
                  onChange={(e) => set({ idle_micro: e.target.checked })}
                />
                <span>Also run small batches when idle during the day</span>
              </label>
              <small className="muted">
                When plugged in and idle for 7 minutes, processes up to two passages and one chat so
                memory keeps up even if nights are missed.
              </small>
            </div>

            <div className="mem-setting-row">
              <label className="check">
                <input
                  type="checkbox"
                  checked={draft.wake}
                  disabled={busy}
                  onChange={(e) => set({ wake: e.target.checked })}
                />
                <span>Wake this computer from sleep to run</span>
              </label>
              <small className="muted">
                Registers a Windows wake timer when plugged in and sleeps again once done.
              </small>
              {draft.wake && timerWarning && <div className="chat-error">{timerWarning}</div>}
              {draft.wake && !dirty && view.settings.wake && !view.wake_task_registered && (
                <div className="chat-error">
                  Wake-up task isn&apos;t registered yet. Save again to retry.
                </div>
              )}
            </div>
          </div>
        )}
      </div>

      <div className="mem-card-actions">
        <button className="primary" disabled={busy || !dirty} onClick={() => void save()}>
          Save schedule
        </button>
        {note && <small className="muted">{note}</small>}
      </div>
      {error && <div className="chat-error">{error}</div>}

      <button
        type="button"
        className="disclosure-toggle"
        aria-expanded={showDevTools}
        onClick={() => setShowDevTools((v) => !v)}
      >
        {showDevTools ? '▾' : '▸'} Developer tools
      </button>
      {showDevTools && (
        <div className="mem-danger-row" style={{ borderTop: 'none', paddingTop: 0 }}>
          <div>
            <div>Screened paragraphs export</div>
            <small className="muted">
              Save recent decision-model judgements as JSONL for offline evaluation
              (docs/MEMORY_OVERNIGHT.md).
            </small>
          </div>
          <button onClick={() => void saveLog()}>Export JSONL…</button>
        </div>
      )}
    </div>
  );
}
