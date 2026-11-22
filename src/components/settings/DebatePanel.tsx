import { useEffect, useMemo, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { DebateSeat, ModelInfo, ProviderAccountView } from '@/lib/types';
import { modelLabel, modelTitle } from '@/lib/models';

/** Settings → Specialists → debater → Edit → Debate panel: the models a
    `/debate` may seat its personas on. Any model a provider lists can sit
    here, checked or not and whether or not a setup uses it, since a debate
    wants capable models. With no seats, a debate uses the setups. Mirrors
    `config::providers::debate`. */
export function DebatePanel() {
  const [seats, setSeats] = useState<DebateSeat[]>([]);
  const [accounts, setAccounts] = useState<ProviderAccountView[]>([]);
  const [providerId, setProviderId] = useState('');
  const [model, setModel] = useState('');
  const [name, setName] = useState('');
  const [error, setError] = useState('');

  useEffect(() => {
    void ipc
      .getDebatePanel()
      .then(setSeats)
      .catch((e) => setError(String(e)));
    void ipc
      .listProviderAccounts()
      .then(setAccounts)
      .catch(() => {});
  }, []);

  const infoOf = useMemo(() => {
    const map = new Map<string, ModelInfo>();
    for (const a of accounts) {
      for (const m of a.model_infos) map.set(`${a.id}\u0000${m.model}`, m);
    }
    return (seat: { provider_id: string; model: string }) =>
      map.get(`${seat.provider_id}\u0000${seat.model}`);
  }, [accounts]);

  const account = accounts.find((a) => a.id === providerId);

  const save = async (next: DebateSeat[]) => {
    try {
      setSeats(await ipc.setDebatePanel(next));
      setError('');
    } catch (e) {
      setError(String(e));
    }
  };

  const add = () => {
    if (!providerId || !model) return;
    if (seats.some((s) => s.provider_id === providerId && s.model === model)) {
      setError('That model already has a seat.');
      return;
    }
    void save([...seats, { id: '', name: name.trim(), provider_id: providerId, model }]);
    setModel('');
    setName('');
  };

  return (
    <>
      <div className="settings-subhead">
        <h3>Debate panel</h3>
        <p className="muted">
          Models a <code>/debate</code> seats its personas on (and <code>/critique</code> its
          lenses). Any model a provider lists; the subagent denylist does not apply. With none, your
          models are used.
        </p>
      </div>
      {error && <div className="chat-error">{error}</div>}
      <div className="ext-list">
        {seats.map((s) => {
          const info = infoOf(s);
          return (
            <div className="row" key={s.id} style={{ alignItems: 'center' }}>
              <div style={{ flex: 1 }} title={info ? modelTitle(info) : s.model}>
                <div>{s.name}</div>
                <div className="muted" style={{ fontSize: 13 }}>
                  {info ? `${info.provider_name} · ${modelLabel(info)}` : s.model}
                  {info?.intelligence != null && ` · intelligence ${Math.round(info.intelligence)}`}
                  {!info && ' · no longer offered'}
                </div>
              </div>
              <button onClick={() => void save(seats.filter((x) => x.id !== s.id))}>Remove</button>
            </div>
          );
        })}
        {seats.length === 0 && <p className="muted">No seats yet.</p>}
      </div>
      <div className="row">
        <select
          value={providerId}
          onChange={(e) => {
            setProviderId(e.target.value);
            setModel('');
          }}
        >
          <option value="">Provider…</option>
          {accounts.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name}
            </option>
          ))}
        </select>
        <select value={model} disabled={!account} onChange={(e) => setModel(e.target.value)}>
          <option value="">Model…</option>
          {account?.model_infos.map((m) => (
            <option key={m.model} value={m.model} title={modelTitle(m)}>
              {modelLabel(m)}
            </option>
          ))}
        </select>
        <input
          value={name}
          placeholder="Name (optional)"
          onChange={(e) => setName(e.target.value)}
        />
        <button disabled={!providerId || !model} onClick={add}>
          Add seat
        </button>
      </div>
    </>
  );
}
