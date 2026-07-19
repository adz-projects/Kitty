import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type {
  AdaptivePathwayExplorationHealth,
  AdaptivePathwayGraphHealth,
  AdaptivePathwayHealthIssue,
  AdaptivePathwayState,
} from '@/lib/types';

const SEVERITY_DOT: Record<string, string> = {
  info: 'ok',
  warning: 'warn',
  error: 'bad',
};

// Every AppState-backed command here goes through `require_ok` on the Rust
// side, which rejects with exactly this message when the sidecar isn't
// running — matched so the pane can show a specific "go enable it" prompt
// instead of a generic error banner or an indefinite spinner.
const SIDECAR_DOWN_MESSAGE = "Adaptive Pathway isn't running";

/** Read-only Graph Health card (Round-D Batch 2, richer data added Round-7
    item 6) — `GET /state` + `GET /health`'s issue list + `GET /graph_health`'s
    edge/tier/hotspot detail. No live edit surface here; this is a status view. */
export function GraphHealth() {
  const [state, setState] = useState<AdaptivePathwayState | null>(null);
  const [issues, setIssues] = useState<AdaptivePathwayHealthIssue[]>([]);
  const [graphHealth, setGraphHealth] = useState<AdaptivePathwayGraphHealth | null>(null);
  const [explorationHealth, setExplorationHealth] =
    useState<AdaptivePathwayExplorationHealth | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);

  const sidecarDown = error.includes(SIDECAR_DOWN_MESSAGE);

  const load = async () => {
    setLoading(true);
    setError('');
    try {
      const [s, h, g] = await Promise.all([
        ipc.adaptivePathwayGetState(),
        ipc.adaptivePathwayHealth(),
        ipc.adaptivePathwayGraphHealth(),
      ]);
      setState(s);
      setIssues(h.issues);
      setGraphHealth(g);
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
      {loading && !sidecarDown && <p className="muted">Loading…</p>}
      {sidecarDown ? (
        <div className="chat-error">
          Sidecar not running — enable Adaptive Pathway first (Settings → Adaptive Pathway).
        </div>
      ) : (
        error && <div className="chat-error">{error}</div>
      )}
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

      {graphHealth && (
        <>
          <h2>Graph</h2>
          <div className="field">
            <div>
              <span className="muted">Total edges:</span> {graphHealth.total_edges}
              <div className="muted" style={{ fontSize: 11 }}>
                How many learned preferences it's tracking in total.
              </div>
            </div>
            <div>
              <span className="muted">High-confidence edges:</span>{' '}
              {(graphHealth.high_confidence_pct * 100).toFixed(1)}%
              <div className="muted" style={{ fontSize: 11 }}>
                Share of those it's confident enough in to act on without hedging.
              </div>
            </div>
            <div>
              <span className="muted">Tier distribution:</span>{' '}
              {Object.entries(graphHealth.tier_distribution ?? {})
                .map(([tier, count]) => `${tier}: ${count}`)
                .join(', ') || 'n/a'}
              <div className="muted" style={{ fontSize: 11 }}>
                How many edges are hot (frequently used), warm, or cold (rarely used).
              </div>
            </div>
            <div>
              <span className="muted">Last override rate:</span>{' '}
              {(graphHealth.last_override_rate * 100).toFixed(1)}%
              <div className="muted" style={{ fontSize: 11 }}>
                How often you've recently overridden its suggestions.
              </div>
            </div>
            <div>
              <span className="muted">Flagged hotspots:</span> {graphHealth.flagged_hotspots}
              <div className="muted" style={{ fontSize: 11 }}>
                Edges it's confident in but that keep getting overridden anyway — worth a look.
              </div>
            </div>
          </div>
          {(graphHealth.hotspot_details ?? []).length > 0 && (
            <div className="field">
              {(graphHealth.hotspot_details ?? []).map((h, i) => (
                <div className="row" key={i} style={{ alignItems: 'flex-start' }}>
                  <span className="status-dot warn" />
                  <div>
                    <div>{String(h.primitive ?? h.edge_id ?? 'hotspot')}</div>
                    <div className="muted" style={{ fontSize: 11 }}>
                      confidence {Number(h.confidence ?? 0).toFixed(2)}, overridden{' '}
                      {(Number(h.override_rate ?? 0) * 100).toFixed(0)}% of the time
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )}
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
