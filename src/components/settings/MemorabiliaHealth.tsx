import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { MemorabiliaStats } from '@/lib/types';

/** Friendly names for the importance buckets in `stats.by_importance` — a
    plain `Record<string, number>` on the wire, so this falls back to the raw
    key for anything not in the known set rather than dropping it silently. */
const IMPORTANCE_LABEL: Record<string, string> = {
  high: 'high importance',
  low: 'low importance',
  unknown: 'importance not yet judged',
};

/** Health view for the memorabilia (factual-memory) engine, rolled into the
    Memorabilia pane as a section rather than its own nav tab — parallel to
    pathway's `GraphHealth`. A read-only counts readout sourced from
    `GET /api/memorabilia/stats`; the correct/delete surface is the fact
    table above it. */
export function MemorabiliaHealth() {
  const [stats, setStats] = useState<MemorabiliaStats | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);

  const load = async () => {
    setLoading(true);
    setError('');
    try {
      const s = await ipc.getMemorabiliaStats();
      // The daemon route returns a soft `{ error }` shape (HTTP 200) while the
      // engine is unavailable — e.g. in the window right after a toggle
      // restarts the backend. Treat anything without the expected numeric
      // fields as "not ready yet" rather than letting the render throw on a
      // missing field (which would blank the whole Settings window).
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
  };

  useEffect(() => void load(), []);

  return (
    <>
      <h2>Health</h2>
      <p className="muted">
        A quick look at what Kitty has remembered. Nothing here needs your attention.
      </p>
      {loading && <p className="muted">Loading…</p>}
      {error && <div className="chat-error">{error}</div>}

      {stats && (
        <div className="field">
          <div>
            Kitty is holding <strong>{stats.active}</strong> fact
            {stats.active === 1 ? '' : 's'}
            {Object.keys(stats.by_importance ?? {}).length > 0 && (
              <>
                {' '}
                &mdash;{' '}
                {Object.entries(stats.by_importance ?? {})
                  .map(([k, count]) => `${count} ${IMPORTANCE_LABEL[k] ?? k}`)
                  .join(', ')}
              </>
            )}
            .
          </div>
          <div className="muted">
            Backed by <strong>{stats.chunks_active}</strong> piece
            {stats.chunks_active === 1 ? '' : 's'} of evidence.
          </div>
          {stats.disputed > 0 && (
            <div>
              <strong>{stats.disputed}</strong> fact{stats.disputed === 1 ? '' : 's'} in dispute
              <div className="muted">
                Conflicting evidence — down-weighted in recall until it settles.
              </div>
            </div>
          )}
          {stats.archived > 0 && (
            <div className="muted">
              {stats.archived} archived (no longer actively supported, kept for reference).
            </div>
          )}
        </div>
      )}

      <button onClick={() => void load()}>Refresh</button>
    </>
  );
}
