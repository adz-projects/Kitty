import { useCallback, useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { PathwayBelief } from '@/lib/types';

type LayerFilter = 'all' | PathwayBelief['layer'] | 'conflicts';

const LAYER_LABEL: Record<PathwayBelief['layer'], string> = {
  identity: 'Identity',
  context: 'Context',
  conversation: 'Chat',
};

const KIND_LABEL: Record<NonNullable<PathwayBelief['kind']>, string> = {
  preference: 'Preference',
  stance: 'Stance',
  context: 'Context',
  trait: 'Trait',
};

/** Searchable card-list browser for everything Adaptive Pathway holds,
    allowing users to filter by layer/conflicts and forget entries cleanly. */
export function BeliefBrowser({
  refreshKey = 0,
  onMutated,
}: {
  refreshKey?: number;
  onMutated?: () => void;
}) {
  const [beliefs, setBeliefs] = useState<PathwayBelief[] | null>(null);
  const [error, setError] = useState('');
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [filter, setFilter] = useState('');
  const [layerFilter, setLayerFilter] = useState<LayerFilter>('all');

  const load = useCallback(() => {
    void ipc
      .getPathwayBeliefs()
      .then((r) => {
        setBeliefs(r.beliefs);
        setError('');
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    load();
  }, [load, refreshKey]);

  const remove = async (id: string) => {
    setDeletingId(id);
    try {
      await ipc.deletePathwayBelief(id);
      setBeliefs((prev) => (prev ? prev.filter((b) => b.id !== id) : prev));
      onMutated?.();
    } catch (e) {
      setError(String(e));
    } finally {
      setDeletingId(null);
    }
  };

  const q = filter.trim().toLowerCase();
  const conflictCount = beliefs?.filter((b) => b.contradict_count > 0).length ?? 0;
  const visible = (beliefs ?? []).filter((b) => {
    if (layerFilter === 'conflicts' && b.contradict_count <= 0) return false;
    if (layerFilter !== 'all' && layerFilter !== 'conflicts' && b.layer !== layerFilter) {
      return false;
    }
    if (!q) return true;
    return (
      b.text.toLowerCase().includes(q) ||
      (b.domain?.toLowerCase().includes(q) ?? false) ||
      (b.kind?.toLowerCase().includes(q) ?? false)
    );
  });

  const filters: { id: LayerFilter; label: string }[] = [
    { id: 'all', label: 'All' },
    { id: 'identity', label: 'Identity' },
    { id: 'context', label: 'Context' },
    { id: 'conversation', label: 'Chat' },
    ...(conflictCount > 0
      ? [{ id: 'conflicts' as const, label: `Conflicts (${conflictCount})` }]
      : []),
  ];

  return (
    <div className="mem-card">
      <div className="mem-card-head">
        <h2>What it remembers</h2>
        {beliefs && beliefs.length > 0 && (
          <small className="muted">
            Showing {visible.length} of {beliefs.length}
          </small>
        )}
      </div>

      {error && <p className="chat-error">{error}</p>}
      {beliefs == null && !error && <p className="muted">Loading…</p>}
      {beliefs && beliefs.length === 0 && <p className="muted">Nothing learned yet.</p>}

      {beliefs && beliefs.length > 0 && (
        <>
          <div className="mem-toolbar">
            <input
              className="mem-filter-input"
              placeholder="Search beliefs or domains…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            <div className="mem-chip-group" role="group" aria-label="Filter by layer">
              {filters.map((f) => (
                <button
                  key={f.id}
                  type="button"
                  className={`mem-chip ${layerFilter === f.id ? 'active' : ''}`}
                  onClick={() => setLayerFilter(f.id)}
                >
                  {f.label}
                </button>
              ))}
            </div>
          </div>

          {visible.length === 0 ? (
            <p className="muted">No matching beliefs.</p>
          ) : (
            <div className="mem-list">
              {visible.map((b) => (
                <div className="mem-item" key={b.id}>
                  <div className="mem-item-body">
                    <div className="mem-item-text">{b.text}</div>
                    <div className="mem-item-meta">
                      <span className="mem-pill mem-pill-accent">{LAYER_LABEL[b.layer]}</span>
                      {b.kind && <span className="mem-pill">{KIND_LABEL[b.kind] ?? b.kind}</span>}
                      <span className={`mem-pill ${b.tested ? 'mem-pill-ok' : ''}`}>
                        {Math.round(b.confidence * 100)}% · {b.tested ? 'Confirmed' : 'Unconfirmed'}
                      </span>
                      {b.domain && <span className="mem-pill">{b.domain}</span>}
                      {b.pinned && <span className="mem-pill mem-pill-accent">Pinned</span>}
                      <span className="mem-pill">
                        {b.support_count}× · {b.distinct_sessions} chat
                        {b.distinct_sessions === 1 ? '' : 's'}
                      </span>
                      {b.contradict_count > 0 && (
                        <span className="mem-pill mem-pill-danger">
                          Conflict ×{b.contradict_count}
                        </span>
                      )}
                    </div>
                  </div>
                  <div className="mem-item-action">
                    <button
                      disabled={deletingId === b.id}
                      onClick={() => void remove(b.id)}
                      title="This wasn't right — forget it permanently"
                    >
                      {deletingId === b.id ? 'Removing…' : 'Forget'}
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}
