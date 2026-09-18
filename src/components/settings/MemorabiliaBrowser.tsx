import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { MemorabiliaItem } from '@/lib/types';

/** Lists the factual-memory items (active propositions) the memorabilia
    engine currently holds — the "what it remembers, and correct it" surface,
    parallel to the pathway `BeliefBrowser` but for substantive facts rather
    than behavioral beliefs. Claim / confidence / importance / disputed are
    exactly what `GET /api/memorabilia/items` returns
    (`BigTinyV2/daemon/src/routes/memorabilia.rs::list_items`). Delete routes
    through `forget_item` (permanent suppression + tombstone of the item's
    supporting evidence, not a bare row delete, so a deleted fact can't be
    silently relearned on the next extraction pass). */
export function MemorabiliaBrowser() {
  const [items, setItems] = useState<MemorabiliaItem[] | null>(null);
  const [error, setError] = useState('');
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [filter, setFilter] = useState('');

  const load = () =>
    void ipc
      .getMemorabiliaItems()
      .then((r) => {
        setItems(r.items);
        setError('');
      })
      .catch((e) => setError(String(e)));

  useEffect(() => {
    load();
  }, []);

  const remove = async (id: string) => {
    setDeletingId(id);
    try {
      await ipc.deleteMemorabiliaItem(id);
      setItems((prev) => (prev ? prev.filter((i) => i.id !== id) : prev));
    } catch (e) {
      setError(String(e));
    } finally {
      setDeletingId(null);
    }
  };

  if (error) return <p className="chat-error">{error}</p>;
  if (items == null) return <p className="muted">Loading…</p>;
  if (items.length === 0) return <p className="muted">Nothing remembered yet.</p>;

  const q = filter.trim().toLowerCase();
  const visible = q ? items.filter((i) => i.claim.toLowerCase().includes(q)) : items;

  return (
    <div className="belief-browser">
      <input
        className="belief-browser-filter"
        placeholder="Filter…"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      <table className="settings-table belief-table">
        <thead>
          <tr>
            <th>Fact</th>
            <th>Importance</th>
            <th>Confidence</th>
            <th>Status</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {visible.map((i) => (
            <tr key={i.id}>
              <td>{i.claim}</td>
              <td>{i.importance}</td>
              <td>{Math.round(i.confidence * 100)}%</td>
              <td>
                {i.disputed && <div className="belief-contradicted">Disputed</div>}
                {i.urgency && i.urgency !== 'none' && <div className="muted">{i.urgency}</div>}
              </td>
              <td>
                <button
                  disabled={deletingId === i.id}
                  onClick={() => void remove(i.id)}
                  title="This wasn't right — forget it"
                >
                  {deletingId === i.id ? 'Removing…' : 'Forget'}
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
