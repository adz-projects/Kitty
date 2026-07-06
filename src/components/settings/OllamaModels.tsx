import { useEffect, useState } from 'react';
import { ipc, onPullProgress } from '@/lib/ipc';
import type { OllamaModel, PullProgress } from '@/lib/types';

function human(bytes: number): string {
  if (!bytes) return '';
  const gb = bytes / 1e9;
  return gb >= 1 ? `${gb.toFixed(1)} GB` : `${(bytes / 1e6).toFixed(0)} MB`;
}

/** Ollama model management: installed list, pull-with-progress, delete, browse. */
export function OllamaModels() {
  const [models, setModels] = useState<OllamaModel[]>([]);
  const [pullName, setPullName] = useState('');
  const [pulls, setPulls] = useState<Record<string, PullProgress>>({});
  const [error, setError] = useState('');

  const refresh = () =>
    ipc
      .ollamaListModels()
      .then(setModels)
      .catch((e) => setError(String(e)));

  useEffect(() => {
    void refresh();
    // This panel is conditionally rendered (mounts/unmounts each time the user
    // switches settings tabs), so the listener + its cleanup timers must be
    // torn down on unmount — otherwise every revisit stacks another listener
    // on top of the last (Stage-1 close-out fix).
    const timers = new Set<ReturnType<typeof setTimeout>>();
    const unlisten = onPullProgress((p) => {
      setPulls((cur) => ({ ...cur, [p.pull_id]: p }));
      if (p.done) {
        void refresh();
        // Drop the finished bar shortly after.
        const t = setTimeout(() => {
          timers.delete(t);
          setPulls((cur) => {
            const next = { ...cur };
            delete next[p.pull_id];
            return next;
          });
        }, 4000);
        timers.add(t);
      }
    });
    return () => {
      void unlisten.then((fn) => fn());
      timers.forEach(clearTimeout);
    };
  }, []);

  const startPull = async () => {
    const name = pullName.trim();
    if (!name) return;
    setError('');
    try {
      await ipc.ollamaPullModel(name);
      setPullName('');
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-section">
      <h1>Ollama Models</h1>

      <div className="field">
        <span>Pull a model</span>
        <div className="row">
          <input
            value={pullName}
            placeholder="e.g. llama3.2:1b"
            onChange={(e) => setPullName(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && void startPull()}
          />
          <button className="primary" onClick={() => void startPull()}>
            Pull
          </button>
          <button onClick={() => void ipc.openPath('https://ollama.com/library')}>
            Browse models
          </button>
        </div>
      </div>

      {Object.values(pulls).map((p) => {
        const pct = p.total && p.completed ? Math.round((p.completed / p.total) * 100) : null;
        return (
          <div className="pull-row" key={p.pull_id}>
            <div className="pull-head">
              <span>{p.model}</span>
              <span className="muted">{p.error ? `error: ${p.error}` : p.status}</span>
            </div>
            <div className="progress">
              <div
                className="progress-bar"
                style={{ width: p.done ? '100%' : pct != null ? `${pct}%` : '30%' }}
              />
            </div>
          </div>
        );
      })}

      {error && <div className="chat-error">{error}</div>}

      <div className="model-list">
        {models.length === 0 && <p className="muted">No models installed.</p>}
        {models.map((m) => (
          <div className="model-row" key={m.name}>
            <div>
              <div className="model-name">{m.name}</div>
              <div className="muted" style={{ fontSize: 12 }}>
                {human(m.size)}
                {m.details?.parameter_size ? ` · ${m.details.parameter_size}` : ''}
              </div>
            </div>
            <button
              onClick={() => {
                if (confirm(`Delete ${m.name}?`)) void ipc.ollamaDeleteModel(m.name).then(refresh);
              }}
            >
              Delete
            </button>
          </div>
        ))}
      </div>
    </section>
  );
}
