import { useEffect, useState } from 'react';
import {
  coverageFraction,
  formatDuration,
  isLive,
  isStalled,
  milestoneKey,
  phaseLabel,
  type ResearchWorker,
} from '@/lib/research';
import { ipc } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import { runForSession, useResearchStore } from '@/stores/researchStore';
import { useChatStore } from '@/stores/chatStore';

const STEP_NAMES: Record<string, string> = {
  plan: 'Plan',
  wave: 'Wave',
  critic: 'Critique',
  write: 'Write-up',
};

/** The current time, ticking once a second while `on`: what makes "last
    activity 12s ago" count up between the engine's heartbeats. */
function useNow(on: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!on) return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [on]);
  return now;
}

/** A chat's deep research run: where it is, what each planner and researcher
    is doing right now, how long since anything happened, coverage, and Stop.
    The engine does the work and reports `research://progress` (at every
    milestone and on a heartbeat); this only renders it. At each milestone the
    engine posts a note in the chat, and at the end its report: the chat is
    reloaded once for each so they appear. */
export function ResearchProgressCard({ sessionId }: { sessionId: string | null }) {
  const init = useResearchStore((s) => s.init);
  const loadForSession = useResearchStore((s) => s.loadForSession);
  const run = useResearchStore((s) => runForSession(s.runs, sessionId));
  const reloaded = useResearchStore((s) => s.reloaded);
  const markReloaded = useResearchStore((s) => s.markReloaded);
  const cancel = useResearchStore((s) => s.cancel);
  const reloadCurrent = useChatStore((s) => s.reloadCurrent);
  const busy = useChatStore((s) => s.busy);
  const cwd = useChatStore((s) => s.cwd);
  const live = run ? isLive(run.status) : false;
  const now = useNow(live);

  useEffect(() => {
    void init();
  }, [init]);
  useEffect(() => {
    if (sessionId) void loadForSession(sessionId);
  }, [sessionId, loadForSession]);

  // Milestone notes and the report are appended by the engine outside any
  // turn this window streamed: reload once for each to show it. Never while
  // this chat is mid-reply; the effect runs again when the reply ends.
  const pendingKey = !run ? null : run.status === 'done' ? run.id : milestoneKey(run);
  useEffect(() => {
    if (!pendingKey || busy || reloaded.includes(pendingKey)) return;
    markReloaded(pendingKey);
    void reloadCurrent();
  }, [pendingKey, busy, reloaded, markReloaded, reloadCurrent]);

  // Planning is shown by the plan card itself; a finished run by its report.
  if (!run || run.status === 'planning' || run.status === 'done') return null;

  const coverage = coverageFraction(run);
  const steps = (run.steps ?? []).filter((s) => s.phase !== 'plan');
  const activity = run.activity;
  const workers = activity?.workers ?? [];
  const last = activity?.last_activity_ms ?? null;
  const stalled = isStalled(run, now);
  const canWatch = !isAndroid();

  const watch = (w: ResearchWorker) =>
    void ipc
      .openNewChatWindow({
        session_id: w.session_id,
        cwd: cwd ?? '',
        spectate: true,
        specialist: w.label,
      })
      .catch(() => {
        // A convenience: the run carries on whether or not it opens.
      });

  return (
    <section className="research-card" aria-label="Deep research progress">
      <div className="research-card-head">
        <strong>{run.title ?? 'Deep research'}</strong>
        {live && (
          <button
            onClick={() => void cancel(run.id)}
            title="Stop the research. What it found stays."
          >
            Stop
          </button>
        )}
      </div>
      <div role="status">{phaseLabel(run)}</div>
      {live && (
        <div className={stalled ? 'research-pulse research-stalled' : 'research-pulse muted'}>
          {stalled
            ? `No activity for ${formatDuration(now - (last ?? now))}. It may be stuck: Stop it, or give it a little longer.`
            : [
                last ? `Last activity ${formatDuration(now - last)} ago` : null,
                activity?.step_started_ms
                  ? `this step ${formatDuration(now - activity.step_started_ms)}`
                  : null,
              ]
                .filter(Boolean)
                .join(' · ')}
        </div>
      )}
      {workers.length > 0 && (
        <ul className="research-workers">
          {workers.map((w) => (
            <li key={w.session_id}>
              <div className="research-worker-head">
                <span className="research-worker-label" title={w.label}>
                  {w.label}
                </span>
                <span className="muted">{formatDuration(now - w.started_ms)}</span>
                {canWatch && (
                  <button
                    className="link"
                    onClick={() => watch(w)}
                    title="Open this one's chat in a new window and follow it live"
                  >
                    Watch
                  </button>
                )}
              </div>
              {w.doing && <div className="muted research-worker-doing">{w.doing}</div>}
            </li>
          ))}
        </ul>
      )}
      {(run.coverage?.length ?? 0) > 0 && (
        <div
          className="research-meter"
          role="meter"
          aria-label="Coverage"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(coverage * 100)}
          title={(run.coverage ?? [])
            .map((c) => `${c.label}: ${c.findings} findings, ${c.corroborated} corroborated`)
            .join('\n')}
        >
          <span style={{ width: `${Math.round(coverage * 100)}%` }} />
        </div>
      )}
      <div className="research-card-stats muted">
        <span>{run.findings ?? 0} findings</span>
        <span>{run.sources ?? 0} sources</span>
        <span>{run.open_leads ?? 0} open leads</span>
        {(run.tangents_paid_off ?? 0) > 0 && <span>{run.tangents_paid_off} tangents paid off</span>}
        {(run.tokens_used ?? 0) > 0 && (
          <span>{(run.tokens_used ?? 0).toLocaleString()} tokens</span>
        )}
      </div>
      {steps.length > 0 && (
        <ol className="research-steps">
          {steps.map((s) => (
            <li key={s.turn}>
              {STEP_NAMES[s.phase] ?? s.phase}
              {s.phase === 'wave' ? ` ${s.turn}` : ''}:{' '}
              {s.status === 'running' ? 'running' : (s.summary ?? s.status)}
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
