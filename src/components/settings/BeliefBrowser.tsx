import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { PathwayBelief } from '@/lib/types';

const LAYER_LABEL: Record<PathwayBelief['layer'], string> = {
  identity: 'Identity',
  context: 'Context',
  conversation: 'Conversation',
};

/** Lists every belief the pathway (behavioral-memory) engine currently
    holds about the user — the "correct me" surface the original design
    called for and that never got built while the engine only shipped a
    tool-hint UI. Confidence/tested/layer/support are exactly what
    `GET /api/pathway/beliefs` returns (`plugins/bigtiny_rust/src/routes/
    pathway.rs::list_beliefs`) — no provenance field exists on the wire, so
    none is shown rather than fabricated. Delete routes through the same
    `forget` semantics the model's own `forget` MCP tool uses (permanent
    suppression + tombstone, not a bare row delete, so a deleted belief
    can't silently be relearned on the next extraction pass). */
export function BeliefBrowser() {
  const [beliefs, setBeliefs] = useState<PathwayBelief[] | null>(null);
  const [error, setError] = useState('');
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [filter, setFilter] = useState('');

  const load = () =>
    void ipc
      .getPathwayBeliefs()
      .then((r) => {
        setBeliefs(r.beliefs);
        setError('');
      })
      .catch((e) => setError(String(e)));

  useEffect(() => {
    load();
  }, []);

  const remove = async (id: string) => {
    setDeletingId(id);
    try {
      await ipc.deletePathwayBelief(id);
      setBeliefs((prev) => (prev ? prev.filter((b) => b.id !== id) : prev));
    } catch (e) {
      setError(String(e));
    } finally {
      setDeletingId(null);
    }
  };

  if (error) return <p className="chat-error">{error}</p>;
  if (beliefs == null) return <p className="muted">Loading…</p>;
  if (beliefs.length === 0) return <p className="muted">Nothing learned yet.</p>;

  const q = filter.trim().toLowerCase();
  const visible = q ? beliefs.filter((b) => b.text.toLowerCase().includes(q)) : beliefs;

  return (
    <div className="belief-browser">
      <input
        className="belief-browser-filter"
        placeholder="Filter…"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      <ul className="belief-list">
        {visible.map((b) => (
          <li key={b.id} className="belief-row">
            <div className="belief-row-main">
              <span className="belief-text">{b.text}</span>
              <div className="belief-row-meta muted">
                <span>{LAYER_LABEL[b.layer]}</span>
                <span>{Math.round(b.confidence * 100)}% confidence</span>
                <span>{b.tested ? 'tested' : 'untested'}</span>
                {b.domain && <span>{b.domain}</span>}
                {b.pinned && <span>pinned</span>}
                {b.contradict_count > 0 && (
                  <span className="belief-contradicted">contradicted ×{b.contradict_count}</span>
                )}
                <span>
                  seen {b.support_count}× across {b.distinct_sessions} session
                  {b.distinct_sessions === 1 ? '' : 's'}
                </span>
              </div>
            </div>
            <button
              disabled={deletingId === b.id}
              onClick={() => void remove(b.id)}
              title="This wasn't right — forget it"
            >
              {deletingId === b.id ? 'Removing…' : 'Forget'}
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
