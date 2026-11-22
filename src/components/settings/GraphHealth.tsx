import { useCallback, useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { STALL_HOURS, describeAge, isStalled } from './memoryHealth';
import type { PathwayStats } from '@/lib/types';

const LAYER_ORDER = ['identity', 'context', 'conversation'] as const;

const LAYER_SHORT: Record<string, string> = {
  identity: 'identity',
  context: 'context',
  conversation: 'chat',
};

const SIGNAL_LABEL: Record<string, string> = {
  challenge: 'Challenge',
  tension: 'Tension',
  fresh_angle: 'Fresh angle',
  worth_testing: 'Assumptions tested',
};

/** Overview metrics and signal telemetry for Adaptive Pathway, rendered as a
    scannable card above the belief browser. */
export function GraphHealth({ refreshKey = 0 }: { refreshKey?: number }) {
  const [stats, setStats] = useState<PathwayStats | null>(null);
  const [fallbackTested, setFallbackTested] = useState<number | null>(null);
  const [fallbackAvgConf, setFallbackAvgConf] = useState<number | null>(null);
  const [fallbackContradicted, setFallbackContradicted] = useState<number>(0);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const s = await ipc.getPathwayStats();
      setStats(s);
      // Fallback for older daemons before aggregate fields were added to /stats.
      if (typeof s.tested !== 'number' || typeof s.avg_confidence !== 'number') {
        const b = await ipc.getPathwayBeliefs();
        const tested = b.beliefs.filter((x) => x.tested).length;
        const avg = b.beliefs.length
          ? b.beliefs.reduce((sum, x) => sum + x.confidence, 0) / b.beliefs.length
          : 0;
        const contradicted = b.beliefs.filter((x) => x.contradict_count > 0).length;
        setFallbackTested(tested);
        setFallbackAvgConf(avg);
        setFallbackContradicted(contradicted);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  const testedCount = stats?.tested ?? fallbackTested ?? 0;
  const avgConfidence = stats?.avg_confidence ?? fallbackAvgConf ?? 0;
  const contradictedCount = stats?.contradicted ?? fallbackContradicted;

  const layerSummary = stats
    ? LAYER_ORDER.filter((l) => (stats.by_layer[l] ?? 0) > 0)
        .map((l) => `${stats.by_layer[l]} ${LAYER_SHORT[l] ?? l}`)
        .join(' · ')
    : '';

  const signalEntries = Object.entries(stats?.signals ?? {}).filter(([, v]) => v.total > 0);
  const staged = stats?.staged;

  return (
    <div className="mem-card">
      <div className="mem-card-head">
        <h2>Overview</h2>
        <button onClick={() => void load()} disabled={loading}>
          {loading ? 'Refreshing…' : 'Refresh'}
        </button>
      </div>

      {error && <div className="chat-error">{error}</div>}
      {!stats && loading && <p className="muted">Loading…</p>}

      {stats && (
        <>
          <div className="mem-stats-grid">
            <div className="mem-stat">
              <span className="mem-stat-label">Stored beliefs</span>
              <span className="mem-stat-value">{stats.total}</span>
              <span className="mem-stat-sub">{layerSummary || 'None yet'}</span>
            </div>

            <div className="mem-stat">
              <span className="mem-stat-label">Confirmed</span>
              <span className="mem-stat-value">{testedCount}</span>
              <span className="mem-stat-sub">
                {stats.total > 0
                  ? `${Math.round(avgConfidence * 100)}% avg confidence`
                  : 'Waiting for signal'}
              </span>
            </div>

            <div className="mem-stat">
              <span className="mem-stat-label">Pending synthesis</span>
              <span className="mem-stat-value">{staged ? staged.pending : 0}</span>
              <span className="mem-stat-sub">
                {staged && staged.pending > 0
                  ? `Across ${staged.sessions} chat${staged.sessions === 1 ? '' : 's'}`
                  : stats.learning && stats.learning.unlearned_sessions > 0
                    ? `${stats.learning.unlearned_sessions} chat${
                        stats.learning.unlearned_sessions === 1 ? '' : 's'
                      } queued`
                    : 'Up to date'}
              </span>
            </div>

            <div className="mem-stat">
              <span className="mem-stat-label">Last learned</span>
              <span className="mem-stat-value" style={{ fontSize: 'var(--fs-md)', paddingTop: 4 }}>
                {stats.learning ? describeAge(stats.learning.last_learn_at) : '—'}
              </span>
              <span className="mem-stat-sub">
                {contradictedCount > 0
                  ? `${contradictedCount} active conflict${contradictedCount === 1 ? '' : 's'}`
                  : 'No conflicts'}
              </span>
            </div>
          </div>

          {signalEntries.length > 0 && (
            <div className="mem-signal-block">
              <div className="mem-signal-heading">
                Directive uptake in recent replies (approximate)
              </div>
              <div className="mem-status-strip">
                {signalEntries.map(([kind, counts]) => {
                  const pct = Math.round((counts.engaged / counts.total) * 100);
                  return (
                    <span
                      key={kind}
                      className={`mem-pill ${pct >= 50 ? 'mem-pill-ok' : ''}`}
                      title={`About ${counts.engaged} of ${counts.total} replies took it up: a word-matching estimate, not a judgement`}
                    >
                      {SIGNAL_LABEL[kind] ?? kind}: {counts.engaged}/{counts.total} ({pct}%)
                    </span>
                  );
                })}
              </div>
            </div>
          )}

          {stats.learning &&
            isStalled(stats.learning.unlearned_sessions, stats.learning.last_learn_at) && (
              <div className="memory-stall" role="status">
                Learning seems to have paused: {stats.learning.unlearned_sessions} chat
                {stats.learning.unlearned_sessions === 1 ? '' : 's'} waiting, with nothing learned
                for over {STALL_HOURS} hours.
              </div>
            )}

          {stats.embedding_migration.pending > 0 && (
            <small className="muted">
              Re-indexing {stats.embedding_migration.pending} belief
              {stats.embedding_migration.pending === 1 ? '' : 's'} in the background.
            </small>
          )}
        </>
      )}
    </div>
  );
}
