import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { PathwayBelief, PathwayStats } from '@/lib/types';

const SUPPORT_BUCKETS: [string, (n: number) => boolean][] = [
  ['1 (single observation)', (n) => n === 1],
  ['2–4', (n) => n >= 2 && n <= 4],
  ['5+', (n) => n >= 5],
];

/** Belief-health view for the pathway (behavioral-memory) engine — replaces
    the old ensemble/edge/schism-era Graph Health card, which had no
    equivalent in the belief model. No live edit surface here (that's the
    belief browser under Settings → Adaptive Pathway); this is a status
    view sourced from `GET /api/pathway/stats` + `GET /api/pathway/beliefs`. */
export function GraphHealth() {
  const [stats, setStats] = useState<PathwayStats | null>(null);
  const [beliefs, setBeliefs] = useState<PathwayBelief[]>([]);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);

  const load = async () => {
    setLoading(true);
    setError('');
    try {
      const [s, b] = await Promise.all([ipc.getPathwayStats(), ipc.getPathwayBeliefs()]);
      setStats(s);
      setBeliefs(b.beliefs);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => void load(), []);

  const tested = beliefs.filter((b) => b.tested).length;
  const contradicted = beliefs.filter((b) => b.contradict_count > 0).length;
  const pinned = beliefs.filter((b) => b.pinned).length;
  const avgConfidence = beliefs.length
    ? beliefs.reduce((sum, b) => sum + b.confidence, 0) / beliefs.length
    : 0;

  return (
    <section className="settings-section">
      <h1>Graph Health</h1>
      <p className="muted">
        Insights (advanced) into what the pathway engine has learned — nothing here needs action
        unless the re-indexing count below stays nonzero for a while.
      </p>
      {loading && <p className="muted">Loading…</p>}
      {error && <div className="chat-error">{error}</div>}

      {stats && (
        <div className="field">
          <div>
            <span className="muted">Total beliefs:</span> {stats.total}
          </div>
          <div>
            <span className="muted">By layer:</span>{' '}
            {Object.entries(stats.by_layer)
              .map(([layer, count]) => `${layer}: ${count}`)
              .join(', ') || 'n/a'}
          </div>
          {stats.embedding_migration.pending > 0 && (
            <div>
              <span className="muted">Re-indexing:</span> {stats.embedding_migration.pending} belief
              {stats.embedding_migration.pending === 1 ? '' : 's'} still being re-embedded for{' '}
              {stats.embedding_migration.current_model} after an embedding-model change.
              <div className="muted" style={{ fontSize: 11 }}>
                Happens automatically in the background — no action needed, this just tells you it's
                in progress.
              </div>
            </div>
          )}
        </div>
      )}

      {beliefs.length > 0 && (
        <>
          <h2>Confidence</h2>
          <div className="field">
            <div>
              <span className="muted">Tested vs. untested:</span> {tested} tested,{' '}
              {beliefs.length - tested} untested
              <div className="muted" style={{ fontSize: 11 }}>
                Untested beliefs are discounted in recall until confirmed.
              </div>
            </div>
            <div>
              <span className="muted">Average confidence:</span> {(avgConfidence * 100).toFixed(0)}%
            </div>
            <div>
              <span className="muted">Pinned:</span> {pinned}
            </div>
            <div>
              <span className="muted">Contradicted:</span> {contradicted}
              <div className="muted" style={{ fontSize: 11 }}>
                Beliefs with at least one open contradiction — never silently resolved, just
                down-weighted until you settle it.
              </div>
            </div>
          </div>

          <h2>Support</h2>
          <div className="field">
            {SUPPORT_BUCKETS.map(([label, test]) => (
              <div key={label}>
                <span className="muted">{label}:</span> {beliefs.filter((b) => test(b.support_count)).length}
              </div>
            ))}
          </div>
        </>
      )}

      <button onClick={() => void load()}>Refresh</button>
    </section>
  );
}
