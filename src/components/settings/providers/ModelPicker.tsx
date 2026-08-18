import { useState } from 'react';
import type { ModelPickerEntry } from '@/lib/types';
import { COST_TIER_LABEL, FILTER_LABEL, visibleModels, type PickerFilter } from './modelPickerUtils';

/** Single-select model list for the provider-add redesign — shown once a
    key has validated successfully (`ProviderForm.tsx`'s "Check key & load
    models" step). Filter chips each cut to a top-10 ranked subset; the
    search box works against the full, uncapped list. `onChange` fires with
    a model id, never more than one selected at a time (a provider profile
    now pins to exactly one model for every type this picker covers). */
export function ModelPicker({
  models,
  value,
  onChange,
}: {
  models: ModelPickerEntry[];
  value: string | null;
  onChange: (id: string) => void;
}) {
  const [filter, setFilter] = useState<PickerFilter | null>(null);
  const [search, setSearch] = useState('');

  const shown = visibleModels(models, filter, search);
  const filters = Object.keys(FILTER_LABEL) as PickerFilter[];

  return (
    <div className="model-picker">
      <div className="row model-picker-filters">
        {filters.map((f) => (
          <button
            key={f}
            type="button"
            className={f === filter ? 'active' : ''}
            // Clicking the active chip again clears back to "no filter" —
            // search and a filter chip are mutually exclusive, so picking
            // one always clears the other.
            onClick={() => {
              setFilter((cur) => (cur === f ? null : f));
              setSearch('');
            }}
          >
            {FILTER_LABEL[f]}
          </button>
        ))}
      </div>
      <input
        type="text"
        placeholder="Search models…"
        value={search}
        onChange={(e) => {
          const q = e.target.value;
          setSearch(q);
          if (q.trim()) setFilter(null);
        }}
      />
      <div className="model-picker-list">
        {shown.length === 0 && <p className="muted">No models found.</p>}
        {shown.map((m) => (
          <button
            key={m.id}
            type="button"
            className={`provider-row model-picker-row${m.id === value ? ' active' : ''}`}
            onClick={() => onChange(m.id)}
            title={m.id}
          >
            <span className="model-picker-name">{m.name}</span>
            <span className="model-picker-meta">
              {m.context_length && (
                <span className="muted">{Math.round(m.context_length / 1000)}K ctx</span>
              )}
              {m.cost_tier ? (
                <span className="status-badge">{COST_TIER_LABEL[m.cost_tier]}</span>
              ) : (
                <span className="muted">cost unknown</span>
              )}
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}
