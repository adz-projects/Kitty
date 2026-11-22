import { useCallback, useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { MemorabiliaStats } from '@/lib/types';
import { STALL_HOURS, describeAge, isStalled } from './memoryHealth';

/** Overview metrics and queue health for Memorabilia, rendered as a scannable
    4-tile card matching Adaptive Pathway's `GraphHealth`. */
export function MemorabiliaHealth({ refreshKey = 0 }: { refreshKey?: number }) {
  const [stats, setStats] = useState<MemorabiliaStats | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const s = await ipc.getMemorabiliaStats();
      if (s && typeof (s as { active?: unknown }).active === 'number') {
        setStats(s);
      } else {
        setStats(null);
        const msg = (s as { error?: string })?.error;
        setError(msg ? `Memory isn't ready yet: ${msg}` : '');
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

  const highCount = stats?.by_importance?.high ?? 0;

  return (
    <div className="mem-card">
      <div className="mem-card-head">
        <h2>Overview</h2>
        <button onClick={() => void load()} disabled={loading}>
          {loading ? 'Refreshing…' : 'Refresh'}
        </button>
      </div>

      {!stats && loading && <p className="muted">Loading…</p>}
      {error && <div className="chat-error">{error}</div>}

      {stats && (
        <>
          <div className="mem-stats-grid">
            <div className="mem-stat">
              <span className="mem-stat-label">Active facts</span>
              <span className="mem-stat-value">{stats.active}</span>
              <span className="mem-stat-sub">
                {highCount > 0 ? `${highCount} high importance` : 'None high importance'}
              </span>
            </div>

            <div className="mem-stat">
              <span className="mem-stat-label">Evidence chunks</span>
              <span className="mem-stat-value">{stats.chunks_active}</span>
              <span className="mem-stat-sub">
                {stats.archived > 0 ? `${stats.archived} archived` : 'Supporting active facts'}
              </span>
            </div>

            <div className="mem-stat">
              <span className="mem-stat-label">Extraction queue</span>
              <span className="mem-stat-value">{stats.extraction?.pending ?? 0}</span>
              <span className="mem-stat-sub">
                {stats.extraction && stats.extraction.pending > 0
                  ? 'Waiting to process'
                  : 'All evidence read'}
              </span>
            </div>

            <div className="mem-stat">
              <span className="mem-stat-label">Last fact found</span>
              <span className="mem-stat-value" style={{ fontSize: 'var(--fs-md)', paddingTop: 4 }}>
                {stats.extraction ? describeAge(stats.extraction.last_extracted_at) : '—'}
              </span>
              <span className="mem-stat-sub">
                {stats.disputed > 0 ? `${stats.disputed} disputed` : 'No open disputes'}
              </span>
            </div>
          </div>

          {stats.extraction &&
            isStalled(stats.extraction.pending, stats.extraction.last_extracted_at) && (
              <div className="memory-stall" role="status">
                Remembering seems to have paused: {stats.extraction.pending} piece
                {stats.extraction.pending === 1 ? '' : 's'} of evidence waiting for over{' '}
                {STALL_HOURS} hours.
                {stats.extraction.last_failure && (
                  <div className="muted">Last error: {stats.extraction.last_failure}</div>
                )}
              </div>
            )}
        </>
      )}
    </div>
  );
}
