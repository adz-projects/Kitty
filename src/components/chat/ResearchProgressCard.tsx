import { useEffect } from 'react';
import { coverageFraction, isLive, phaseLabel } from '@/lib/research';
import { runForSession, useResearchStore } from '@/stores/researchStore';
import { useChatStore } from '@/stores/chatStore';

const STEP_NAMES: Record<string, string> = {
  plan: 'Plan',
  wave: 'Wave',
  critic: 'Critique',
  write: 'Write-up',
};

/** A chat's deep research run: where it is, what it has found, how much of
    the plan's coverage it has reached, and Stop. The engine does the work and
    reports `research://progress`; this only renders it. When the report lands
    in the chat, the chat is reloaded once so it appears. */
export function ResearchProgressCard({ sessionId }: { sessionId: string | null }) {
  const init = useResearchStore((s) => s.init);
  const loadForSession = useResearchStore((s) => s.loadForSession);
  const run = useResearchStore((s) => runForSession(s.runs, sessionId));
  const reloaded = useResearchStore((s) => (run ? s.reloaded.includes(run.id) : true));
  const markReloaded = useResearchStore((s) => s.markReloaded);
  const cancel = useResearchStore((s) => s.cancel);
  const reloadCurrent = useChatStore((s) => s.reloadCurrent);

  useEffect(() => {
    void init();
  }, [init]);
  useEffect(() => {
    if (sessionId) void loadForSession(sessionId);
  }, [sessionId, loadForSession]);

  // The report is appended to the chat by the engine, outside any turn this
  // window streamed: reload once to show it.
  useEffect(() => {
    if (run && run.status === 'done' && !reloaded) {
      markReloaded(run.id);
      void reloadCurrent();
    }
  }, [run, reloaded, markReloaded, reloadCurrent]);

  // Planning is shown by the plan card itself; a finished run by its report.
  if (!run || run.status === 'planning' || run.status === 'done') return null;

  const live = isLive(run.status);
  const coverage = coverageFraction(run);
  const steps = (run.steps ?? []).filter((s) => s.phase !== 'plan');

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
