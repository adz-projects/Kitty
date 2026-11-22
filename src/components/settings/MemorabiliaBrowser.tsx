import { useCallback, useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { MemorabiliaItem } from '@/lib/types';

type FactFilter = 'all' | 'high' | 'disputed';

/** Searchable card-list browser for Memorabilia facts with quick importance
    and dispute filters plus permanent forget actions. */
export function MemorabiliaBrowser({
  refreshKey = 0,
  onMutated,
}: {
  refreshKey?: number;
  onMutated?: () => void;
}) {
  const [items, setItems] = useState<MemorabiliaItem[] | null>(null);
  const [error, setError] = useState('');
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [filter, setFilter] = useState('');
  const [factFilter, setFactFilter] = useState<FactFilter>('all');

  const load = useCallback(() => {
    void ipc
      .getMemorabiliaItems()
      .then((r) => {
        if (r && Array.isArray(r.items)) {
          setItems(r.items);
          setError('');
        } else {
          setItems([]);
          const msg = (r as { error?: string })?.error;
          setError(msg ? `Memory isn't ready yet: ${msg}` : '');
        }
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    load();
  }, [load, refreshKey]);

  const remove = async (id: string) => {
    setDeletingId(id);
    try {
      await ipc.deleteMemorabiliaItem(id);
      setItems((prev) => (prev ? prev.filter((i) => i.id !== id) : prev));
      onMutated?.();
    } catch (e) {
      setError(String(e));
    } finally {
      setDeletingId(null);
    }
  };

  const q = filter.trim().toLowerCase();
  const disputedCount = items?.filter((i) => i.disputed).length ?? 0;
  const visible = (items ?? []).filter((i) => {
    if (factFilter === 'high' && i.importance !== 'high') return false;
    if (factFilter === 'disputed' && !i.disputed) return false;
    if (!q) return true;
    return i.claim.toLowerCase().includes(q) || i.importance.toLowerCase().includes(q);
  });

  const filters: { id: FactFilter; label: string }[] = [
    { id: 'all', label: 'All' },
    { id: 'high', label: 'High importance' },
    ...(disputedCount > 0
      ? [{ id: 'disputed' as const, label: `Disputed (${disputedCount})` }]
      : []),
  ];

  return (
    <div className="mem-card">
      <div className="mem-card-head">
        <h2>What it remembers</h2>
        {items && items.length > 0 && (
          <small className="muted">
            Showing {visible.length} of {items.length}
          </small>
        )}
      </div>

      {error && <p className="chat-error">{error}</p>}
      {items == null && !error && <p className="muted">Loading…</p>}
      {items && items.length === 0 && <p className="muted">Nothing remembered yet.</p>}

      {items && items.length > 0 && (
        <>
          <div className="mem-toolbar">
            <input
              className="mem-filter-input"
              placeholder="Search remembered facts…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            <div className="mem-chip-group" role="group" aria-label="Filter facts">
              {filters.map((f) => (
                <button
                  key={f.id}
                  type="button"
                  className={`mem-chip ${factFilter === f.id ? 'active' : ''}`}
                  onClick={() => setFactFilter(f.id)}
                >
                  {f.label}
                </button>
              ))}
            </div>
          </div>

          {visible.length === 0 ? (
            <p className="muted">No matching facts.</p>
          ) : (
            <div className="mem-list">
              {visible.map((i) => (
                <div className="mem-item" key={i.id}>
                  <div className="mem-item-body">
                    <div className="mem-item-text">{i.claim}</div>
                    <div className="mem-item-meta">
                      <span
                        className={`mem-pill ${i.importance === 'high' ? 'mem-pill-accent' : ''}`}
                      >
                        {i.importance} importance
                      </span>
                      <span className="mem-pill">{Math.round(i.confidence * 100)}% confidence</span>
                      {i.disputed && <span className="mem-pill mem-pill-danger">Disputed</span>}
                      {i.urgency && i.urgency !== 'none' && i.urgency !== 'unknown' && (
                        <span className="mem-pill mem-pill-warn">{i.urgency}</span>
                      )}
                    </div>
                  </div>
                  <div className="mem-item-action">
                    <button
                      disabled={deletingId === i.id}
                      onClick={() => void remove(i.id)}
                      title="This wasn't right — forget it permanently"
                    >
                      {deletingId === i.id ? 'Removing…' : 'Forget'}
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
