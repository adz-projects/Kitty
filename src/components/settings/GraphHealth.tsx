import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { PathwayBelief, PathwayStats } from '@/lib/types';

const SUPPORT_BUCKETS: [string, (n: number) => boolean][] = [
  ['Mentioned once', (n) => n === 1],
  ['Mentioned a few times (2–4)', (n) => n >= 2 && n <= 4],
  ['Mentioned often (5+)', (n) => n >= 5],
];

/** Friendly names for the belief layers in `stats.by_layer` — a plain
    `Record<string, number>` on the wire, so this falls back to the raw key
    for anything not in the known set rather than dropping it silently. */
const LAYER_LABEL: Record<string, string> = {
  identity: 'about you',
  context: 'about your current situation',
  conversation: 'from this conversation',
};

/** Belief-health view for the pathway (behavioral-memory) engine, rolled
    into Adaptive Pathway as a section (release-fixes item 23) rather than
    its own nav tab — a stats readout on its own didn't need a full page,
    and its old copy ("by-layer breakdown", "contradicted count") assumed
    more familiarity with the engine's internals than a settings page
    should. Sourced from `GET /api/pathway/stats` + `GET /api/pathway/beliefs`;
    no live edit surface here — that's the belief table above. */
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
    <>
      <h2>Health</h2>
      <p className="muted">
        A quick look at what Kitty has learned. Nothing here needs your attention.
      </p>
      {loading && <p className="muted">Loading…</p>}
      {error && <div className="chat-error">{error}</div>}

      {stats && (
        <div className="field">
          <div>
            Kitty has learned <strong>{stats.total}</strong> thing{stats.total === 1 ? '' : 's'}{' '}
            about you
            {Object.keys(stats.by_layer).length > 0 && (
              <>
                {' '}
                &mdash;{' '}
                {Object.entries(stats.by_layer)
                  .map(([layer, count]) => `${count} ${LAYER_LABEL[layer] ?? layer}`)
                  .join(', ')}
              </>
            )}
            .
          </div>
          {stats.embedding_migration.pending > 0 && (
            <div className="muted">
              Double-checking {stats.embedding_migration.pending} belief
              {stats.embedding_migration.pending === 1 ? '' : 's'} after an update — happens
              automatically in the background, no action needed.
            </div>
          )}
        </div>
      )}

      {beliefs.length > 0 && (
        <>
          <h3>How sure Kitty is</h3>
          <div className="field">
            <div>
              <strong>{tested}</strong> confirmed, <strong>{beliefs.length - tested}</strong> not
              yet confirmed
              <div className="muted">
                Unconfirmed beliefs count less until you or the conversation confirms them.
              </div>
            </div>
            <div>
              <strong>{(avgConfidence * 100).toFixed(0)}%</strong> average confidence
            </div>
            <div>
              <strong>{pinned}</strong> marked to always keep in mind
            </div>
            {contradicted > 0 && (
              <div>
                <strong>{contradicted}</strong> conflict with something learned since
                <div className="muted">
                  Not resolved automatically — down-weighted until you settle it.
                </div>
              </div>
            )}
          </div>

          <h3>How often each was mentioned</h3>
          <div className="field">
            {SUPPORT_BUCKETS.map(([label, test]) => (
              <div key={label}>
                <strong>{beliefs.filter((b) => test(b.support_count)).length}</strong> {label}
              </div>
            ))}
          </div>
        </>
      )}

      <button onClick={() => void load()}>Refresh</button>
    </>
  );
}
