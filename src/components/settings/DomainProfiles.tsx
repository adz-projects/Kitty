import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { AdaptivePathwayDomain } from '@/lib/types';
import { Modal } from '@/components/shared/Modal';
import { LockIcon } from '@/components/icons/LockIcon';

/** Domain Profiles tab (Round-D Batch 2) — `GET /domains` + `PUT
    /domains/{id}`, the one admin tab with real, complete backend support
    beyond the ensemble-weight sliders already on the main settings page. */
export function DomainProfiles() {
  const [domains, setDomains] = useState<AdaptivePathwayDomain[]>([]);
  const [error, setError] = useState('');
  const [editing, setEditing] = useState<AdaptivePathwayDomain | null>(null);
  const [form, setForm] = useState({ name: '', dpp: 1, lambda: 0.5, locked: false });
  const [saving, setSaving] = useState(false);

  const load = async () => {
    try {
      setDomains(await ipc.adaptivePathwayListDomains());
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => void load(), []);

  const openEdit = (d: AdaptivePathwayDomain) => {
    setEditing(d);
    setForm({
      name: d.name,
      dpp: d.dpp_diversity_weight,
      lambda: d.novelty_lambda,
      locked: d.locked,
    });
  };

  const save = async () => {
    if (!editing) return;
    setSaving(true);
    try {
      await ipc.adaptivePathwayUpdateDomain(
        editing.id,
        form.name,
        form.dpp,
        form.lambda,
        form.locked
      );
      setEditing(null);
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="settings-section">
      <h1>Domain Profiles</h1>
      <p className="muted">
        Kitty groups what it learns by topic area — a "domain" — so preferences from one kind of
        work don't bleed into another.
      </p>
      {error && <div className="chat-error">{error}</div>}
      {domains.length === 0 && !error && <p className="muted">No domains learned yet.</p>}
      <div className="ext-list">
        {domains.map((d) => (
          <div className="row" key={d.id} style={{ alignItems: 'center' }}>
            <div style={{ flex: 1 }}>
              <div>
                {d.name} {d.locked && <LockIcon />}
              </div>
              <div className="muted" style={{ fontSize: 11 }}>
                DPP {d.dpp_diversity_weight.toFixed(2)} · λ {d.novelty_lambda.toFixed(2)} ·{' '}
                {d.edge_count} edges · {d.sessions} sessions · override rate{' '}
                {(d.override_rate * 100).toFixed(0)}%
              </div>
            </div>
            <button onClick={() => openEdit(d)}>Edit</button>
          </div>
        ))}
      </div>

      {editing && (
        <Modal title={`Edit domain: ${editing.id}`}>
          <div className="field">
            <span>Name</span>
            <input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} />
          </div>
          <div className="field param-slider">
            <span>DPP diversity weight</span>
            <small className="muted">
              Higher = more variety in what gets suggested for this domain.
            </small>
            <div className="row">
              <input
                type="range"
                min={0}
                max={2}
                step={0.05}
                value={form.dpp}
                onChange={(e) => setForm({ ...form, dpp: Number(e.target.value) })}
              />
              <span className="status-badge">{form.dpp.toFixed(2)}</span>
            </div>
          </div>
          <div className="field param-slider">
            <span>Novelty lambda</span>
            <small className="muted">
              Higher = more willing to try something untested instead of the safe choice.
            </small>
            <div className="row">
              <input
                type="range"
                min={0}
                max={1}
                step={0.05}
                value={form.lambda}
                onChange={(e) => setForm({ ...form, lambda: Number(e.target.value) })}
              />
              <span className="status-badge">{form.lambda.toFixed(2)}</span>
            </div>
          </div>
          <label className="check">
            <input
              type="checkbox"
              checked={form.locked}
              onChange={(e) => setForm({ ...form, locked: e.target.checked })}
            />
            <span>Lock (prevent weekly re-inference from overwriting these values)</span>
          </label>
          <div className="row">
            <button className="primary" disabled={saving} onClick={() => void save()}>
              {saving ? 'Saving…' : 'Save'}
            </button>
            <button onClick={() => setEditing(null)}>Cancel</button>
          </div>
        </Modal>
      )}
    </section>
  );
}
