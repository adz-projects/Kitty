import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type {
  AdaptivePathwayExplorationHealth,
  AdaptivePathwayHealthIssue,
  AdaptivePathwayState,
} from '@/lib/types';

const SEVERITY_DOT: Record<string, string> = {
  info: 'ok',
  warning: 'warn',
  error: 'bad',
};

/** Read-only Graph Health card (Round-D Batch 2) — `GET /state` + `GET
    /health`'s issue list. No live edit surface here; this is a status view. */
export function GraphHealth() {
  const [state, setState] = useState<AdaptivePathwayState | null>(null);
  const [issues, setIssues] = useState<AdaptivePathwayHealthIssue[]>([]);
  const [explorationHealth, setExplorationHealth] =
    useState<AdaptivePathwayExplorationHealth | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);

  const load = async () => {
    setLoading(true);
    setError('');
    try {
      const [s, h] = await Promise.all([
        ipc.adaptivePathwayGetState(),
        ipc.adaptivePathwayHealth(),
      ]);
      setState(s);
      setIssues(h.issues);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
    // Exploration health is an optional enhancement (needs a newer sidecar's
    // `/metrics.exploration_health`). Fetched separately so a failure — or an
    // older sidecar missing the block — only hides this section rather than
    // blanking the core state/issues card above.
    try {
      const m = await ipc.adaptivePathwayGetMetrics();
      setExplorationHealth(m.metrics.exploration_health);
    } catch {
      setExplorationHealth(null);
    }
  };

  useEffect(() => void load(), []);

  return (
    <section className="settings-section">
      <h1>Graph Health</h1>
      <p className="muted">
        Insights (advanced) into how Adaptive Pathway is learning — nothing here needs action unless
        an issue shows up below.
      </p>
      {loading && <p className="muted">Loading…</p>}
      {error && <div className="chat-error">{error}</div>}
      {state && (
        <div className="field">
          <div>
            <span className="muted">Domains:</span> {state.domain_count}
            <div className="muted" style={{ fontSize: 11 }}>
              How many separate topic areas it's tracking preferences for.
            </div>
          </div>
          <div>
            <span className="muted">Feature utilization:</span>{' '}
            {(state.feature_utilization * 100).toFixed(1)}%
            <div className="muted" style={{ fontSize: 11 }}>
              How much of its learned signal space is actually in use.
            </div>
          </div>
          <div>
            <span className="muted">Feature collision rate:</span>{' '}
            {(state.feature_collision_rate * 100).toFixed(1)}%
            <div className="muted" style={{ fontSize: 11 }}>
              How often distinct signals get mixed up with each other — lower is better.
            </div>
          </div>
          <div>
            <span className="muted">Ensemble schism state:</span> {state.schism_state}
            <div className="muted" style={{ fontSize: 11 }}>
              Whether its models currently agree (`none`) or have split into conflicting patterns.
            </div>
          </div>
          <div>
            <span className="muted">Plateau risk:</span> {state.plateau_risk_score.toFixed(2)}
            <div className="muted" style={{ fontSize: 11 }}>
              How likely it is to keep suggesting the same things instead of trying something new.
            </div>
          </div>
          <div>
            <span className="muted">Warm cache ready:</span> {state.warm_ready ? 'Yes' : 'No'}
            <div className="muted" style={{ fontSize: 11 }}>
              Whether its learned data is loaded and ready, or still warming up.
            </div>
          </div>
        </div>
      )}

      {explorationHealth && (
        <>
          <h2>Exploration mix</h2>
          <div className="field">
            <div>
              <span className="muted">% of hints from exploration models:</span>{' '}
              {(explorationHealth.ig_pc_hint_ratio * 100).toFixed(1)}%
              <div className="muted" style={{ fontSize: 11 }}>
                How often a suggestion came from a model actively looking for something new to try,
                rather than the safe/familiar choice.
              </div>
            </div>
            <div>
              <span className="muted">Action entropy (last 50 turns):</span>{' '}
              {explorationHealth.action_entropy_50w.toFixed(3)}
              <div className="muted" style={{ fontSize: 11 }}>
                How varied its recent suggestions have been — low means it's repeating itself.
              </div>
            </div>
            <div>
              <span className="muted">Unique primitives active:</span>{' '}
              {explorationHealth.unique_primitives_active}
              <div className="muted" style={{ fontSize: 11 }}>
                How many distinct kinds of actions it's currently drawing suggestions from.
              </div>
            </div>
            <div>
              <span className="muted">Wildcards surfaced this session:</span>{' '}
              {explorationHealth.wildcard_slot_used}
              <div className="muted" style={{ fontSize: 11 }}>
                How many untested-angle suggestions it's offered you this session.
              </div>
            </div>
          </div>
        </>
      )}

      <h2>Issues</h2>
      {issues.length === 0 && !loading && <p className="muted">No issues.</p>}
      {issues.map((issue, i) => (
        <div className="row" key={i} style={{ alignItems: 'flex-start' }}>
          <span className={`status-dot ${SEVERITY_DOT[issue.severity] ?? 'warn'}`} />
          <div>
            <div>{issue.message}</div>
            <div className="muted" style={{ fontSize: 11 }}>
              {issue.component}
            </div>
          </div>
        </div>
      ))}

      <button onClick={() => void load()}>Refresh</button>
    </section>
  );
}
